//! End-to-end checks for the assistant's observation tools against real
//! service state: an EA link answered through its own poll endpoint, an
//! in-memory audit trail with explicit record times, and a pinned clock.
//! Nothing here reaches a network, a database, or a model provider.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use actix_web::test as awtest;
use async_trait::async_trait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::Tool;
use super::bounded::{MAX_OUTPUT_CHARS, serialized_chars};
use crate::AppState;
use crate::audit::{AuditEvent, AuditKind, AuditRuntime, AuditTrail, MemoryTrail};
use crate::broker::{
    AccountLogin, AccountSnapshot, AccountSnapshotPayload, BrokerRuntime, BrokerSettings,
    CloseOrderRequest, CommandKind, EaLink, ModifyOrderRequest, ORDER_MAGIC, PositionKind,
    PositionPayload, ServerName, Symbol,
};
use crate::calendar::{
    CalendarError, CalendarEvent, CalendarProvider, CalendarRuntime, EventCalendar, Impact,
};
use crate::config::{ConfigError, ServiceConfig};
use crate::market::{
    Candle, CandleRequest, CandleSeries, MarketError, MarketFeed, MarketProvider, MarketRuntime,
};
use crate::risk::{RiskGate, RiskPolicy};

const TOKEN: &str = "test-token-1234567890";
/// Broker server clock: UTC+2, as on the live terminal.
const BROKER_OFFSET: i64 = 7_200;
const TICKET: i64 = 10_654_130;
const CMD_OPEN: &str = "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10";
const CMD_MODIFY: &str = "6b4f6d2f-3c2e-4b68-8e38-8c1e3f8f9b21";
const CMD_CLOSE: &str = "7c5a7e3a-4d3f-4c79-9f49-9d2f4a9a0c32";

/// Unix seconds of an RFC 3339 UTC instant.
fn utc(text: &str) -> i64 {
    OffsetDateTime::parse(text, &Rfc3339)
        .expect("fixture time")
        .unix_timestamp()
}

/// Broker-clock reading the terminal would stamp for a UTC instant.
fn broker(text: &str) -> i64 {
    utc(text) + BROKER_OFFSET
}

/// 2025-03-12T10:00:00Z (12:00 in UTC+2): in the past, so the retained
/// snapshot's age is zero and its receipt time is exactly this instant.
fn now() -> i64 {
    utc("2025-03-12T10:00:00Z")
}

fn at(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).expect("positive"))
}

fn config() -> ServiceConfig {
    ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("test config")
}

fn position(ticket: i64, opened_utc: &str) -> PositionPayload {
    PositionPayload {
        ticket,
        symbol: "USDJPY".to_owned(),
        magic: ORDER_MAGIC,
        kind: PositionKind::Buy,
        lots: 0.01,
        price: 156.198,
        profit: 0.84,
        stop_loss: 155.9,
        take_profit: 156.8,
        opened_at: broker(opened_utc),
        current: 156.3,
        swap: -0.02,
        commission: 0.0,
    }
}

struct Fixture {
    state: AppState,
    trail: Arc<MemoryTrail>,
    link: Arc<EaLink>,
}

/// EA-backed state with an audit trail and, optionally, a retained snapshot
/// whose server clock runs two hours ahead of UTC.
fn fixture(snapshot: bool, positions: Vec<PositionPayload>) -> Fixture {
    fixture_with(snapshot.then(|| now() + BROKER_OFFSET), positions)
}

/// Like [`fixture`], with an explicit snapshot `serverTime` (or none).
fn fixture_with(server_time: Option<i64>, positions: Vec<PositionPayload>) -> Fixture {
    let settings = BrokerSettings::from_source(|name| match name {
        "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
        "VEYRA_EA_TOKEN" => Ok(TOKEN.to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("settings")
    .expect("configured");
    let runtime = BrokerRuntime::from_settings(settings).expect("runtime");
    let link = runtime.ea_link().expect("ea link");
    link.record(AccountSnapshot::new(
        AccountLogin::parse(94168).expect("login"),
        ServerName::parse("IFCMarkets-Real").expect("server"),
        Symbol::parse("USDJPY").expect("symbol"),
        true,
        true,
        0,
        0.0,
    ));
    if let Some(server_time) = server_time {
        link.retain_snapshot(AccountSnapshotPayload {
            balance: 20.57,
            equity: 21.41,
            free_margin: 18.0,
            orders: u32::try_from(positions.len()).expect("count"),
            lots: 0.01,
            positions,
            positions_truncated: false,
            server_time,
            leverage: 100,
            margin_level: 0.0,
        });
    }
    let trail = Arc::new(MemoryTrail::default());
    let state = AppState::new(
        config(),
        Some(runtime),
        None,
        RiskGate::new(RiskPolicy::default()),
    )
    .with_audit(Some(AuditRuntime::new(trail.clone())))
    .with_fixed_now(Some(at(now())));
    Fixture { state, trail, link }
}

/// State with an audit trail and no broker at all.
fn audit_only() -> (AppState, Arc<MemoryTrail>) {
    let trail = Arc::new(MemoryTrail::default());
    let state = AppState::new(config(), None, None, RiskGate::new(RiskPolicy::default()))
        .with_audit(Some(AuditRuntime::new(trail.clone())))
        .with_fixed_now(Some(at(now())));
    (state, trail)
}

fn bare() -> AppState {
    AppState::new(config(), None, None, RiskGate::new(RiskPolicy::default()))
        .with_fixed_now(Some(at(now())))
}

/// Plays the terminal: polls until the next command, checks its kind, and
/// acknowledges it with `data` (or fails it with `error`).
async fn answer_next(link: &Arc<EaLink>, kind: &str, reply: Result<Value, &str>) -> Value {
    let app = awtest::init_service(crate::broker::ea::create_ea_app(link.clone())).await;
    let hello = json!({
        "t": "hb", "token": TOKEN, "acct": 94168, "server": "IFCMarkets-Real",
        "symbol": "USDJPY", "connected": true, "tradeAllowed": true, "orders": 0, "lots": 0.0
    });
    for _ in 0..200 {
        let response = awtest::call_service(
            &app,
            awtest::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(hello.to_string())
                .to_request(),
        )
        .await;
        let body: Value = awtest::read_body_json(response).await;
        if body["t"] == "cmd" {
            assert_eq!(body["kind"], kind, "delivered command kind");
            let ack = match &reply {
                Ok(data) => {
                    json!({"t": "ack", "token": TOKEN, "id": body["id"], "ok": true, "data": data})
                }
                Err(error) => {
                    json!({"t": "ack", "token": TOKEN, "id": body["id"], "ok": false, "error": error})
                }
            };
            let response = awtest::call_service(
                &app,
                awtest::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(ack.to_string())
                    .to_request(),
            )
            .await;
            assert!(response.status().is_success(), "ack accepted");
            return body;
        }
        actix_web::rt::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no {kind} command was delivered");
}

/// Runs one tool while the terminal answers its history request.
async fn with_history(
    fixture: &Fixture,
    tool: Tool,
    arguments: Value,
    orders: Vec<Value>,
    truncated: bool,
) -> (Result<Value, String>, Value) {
    let state = fixture.state.clone();
    let task = actix_web::rt::spawn(async move { tool.read(&state, &arguments).await });
    let total = orders.len();
    let command = answer_next(
        &fixture.link,
        "order_history",
        Ok(json!({"orders": orders, "total": total, "truncated": truncated})),
    )
    .await;
    (task.await.expect("tool task joins"), command)
}

fn trade(ticket: i64, symbol: &str, kind: &str, open: &str, close: &str, profit: f64) -> Value {
    json!({
        "ticket": ticket, "symbol": symbol, "kind": kind, "lots": 0.01,
        "openPrice": 156.198, "closePrice": 156.41,
        "openTime": broker(open), "closeTime": broker(close),
        "profit": profit, "swap": 0.0, "commission": 0.0, "magic": ORDER_MAGIC
    })
}

/// Three closes around the operator's (UTC+2) midnight, newest first.
fn closes() -> Vec<Value> {
    vec![
        // 08:46:06 local on the 12th: today everywhere.
        trade(
            TICKET,
            "USDJPY",
            "buy",
            "2025-03-12T04:30:06Z",
            "2025-03-12T06:46:06Z",
            1.36,
        ),
        // 00:30 local on the 12th, still the 11th in UTC.
        trade(
            10_654_100,
            "EURUSD",
            "sell",
            "2025-03-11T20:00:00Z",
            "2025-03-11T22:30:00Z",
            -0.8,
        ),
        // 23:50 local on the 11th: yesterday everywhere.
        trade(
            10_654_000,
            "USDJPY",
            "buy",
            "2025-03-11T19:00:00Z",
            "2025-03-11T21:50:00Z",
            0.5,
        ),
    ]
}

fn tickets(result: &Value, field: &str) -> Vec<i64> {
    result[field]
        .as_array()
        .expect("list")
        .iter()
        .map(|row| row["ticket"].as_i64().expect("ticket"))
        .collect()
}

fn assert_bounded(result: &Value) {
    assert!(
        serialized_chars(result) <= MAX_OUTPUT_CHARS,
        "tool output must fit the ceiling: {} chars",
        serialized_chars(result)
    );
}

#[actix_web::test]
async fn closed_trades_answers_which_positions_closed_today_in_the_operator_day() {
    let fixture = fixture(true, Vec::new());
    let (result, command) = with_history(
        &fixture,
        Tool::ClosedTrades,
        json!({"since": "today", "utc_offset_minutes": 120}),
        closes(),
        false,
    )
    .await;
    let result = result.expect("closed trades");
    assert_eq!(
        command["history"]["days"], 2,
        "today needs a two-day look-back"
    );
    assert_eq!(command["history"]["magic"], ORDER_MAGIC);
    assert_eq!(tickets(&result, "trades"), vec![TICKET, 10_654_100]);
    assert_eq!(result["window"]["since_utc"], "2025-03-11T22:00:00Z");
    assert_eq!(result["window"]["since_local"], "2025-03-12T00:00:00+02:00");
    assert_eq!(result["window"]["until_utc"], "2025-03-12T10:00:00Z");
    assert_eq!(result["broker_clock"]["utc_offset"], "+02:00");
    let first = &result["trades"][0];
    assert_eq!(first["side"], "Long");
    assert_eq!(first["closed_utc"], "2025-03-12T06:46:06Z");
    assert_eq!(first["closed_local"], "2025-03-12T08:46:06+02:00");
    assert_eq!(first["held"], "2h 16m");
    assert_eq!(first["net"], 1.36);
    assert_eq!(result["trades"][1]["side"], "Short");
    assert_eq!(result["totals"]["count"], 2);
    assert_eq!(result["totals"]["wins"], 1);
    assert_eq!(result["totals"]["losses"], 1);
    assert_eq!(result["totals"]["net"], 0.56);
    assert_eq!(result["history_truncated"], false);
    assert!(result.get("trades_omitted").is_none());
    assert_bounded(&result);

    // Without an operator offset the day is the UTC day.
    let (result, _) = with_history(
        &fixture,
        Tool::ClosedTrades,
        json!({"since": "today"}),
        closes(),
        false,
    )
    .await;
    let result = result.expect("utc day");
    assert_eq!(tickets(&result, "trades"), vec![TICKET]);
    assert_eq!(result["window"]["operator_utc_offset_minutes"], Value::Null);
    assert!(result["trades"][0].get("closed_local").is_none());

    // Yesterday, exactly: the half-open local day excludes 00:30 on the 12th.
    let (result, _) = with_history(
        &fixture,
        Tool::ClosedTrades,
        json!({"since": "yesterday", "until": "yesterday", "utc_offset_minutes": 120}),
        closes(),
        false,
    )
    .await;
    assert_eq!(
        tickets(&result.expect("yesterday"), "trades"),
        vec![10_654_000]
    );

    // A symbol filter is case-insensitive.
    let (result, _) = with_history(
        &fixture,
        Tool::ClosedTrades,
        json!({"since": "yesterday", "symbol": "usdjpy", "utc_offset_minutes": 120}),
        closes(),
        false,
    )
    .await;
    assert_eq!(
        tickets(&result.expect("symbol"), "trades"),
        vec![TICKET, 10_654_000]
    );
}

#[actix_web::test]
async fn closed_trades_reports_empty_windows_and_bounds_large_histories() {
    let fixture = fixture(true, Vec::new());
    let (result, _) = with_history(
        &fixture,
        Tool::ClosedTrades,
        json!({"since": "2025-03-12T09:00:00Z"}),
        closes(),
        false,
    )
    .await;
    let result = result.expect("empty window");
    assert_eq!(result["trades"], json!([]));
    assert_eq!(result["totals"]["count"], 0);
    assert_eq!(result["totals"]["net"], 0.0);

    // 256 closes today, and the terminal says it capped the list.
    let many: Vec<Value> = (0..256)
        .map(|index| {
            trade(
                20_000_000 - index,
                "USDJPY",
                "buy",
                "2025-03-12T01:00:00Z",
                "2025-03-12T09:00:00Z",
                0.1,
            )
        })
        .collect();
    let (result, command) =
        with_history(&fixture, Tool::ClosedTrades, json!({"days": 3}), many, true).await;
    let result = result.expect("large history");
    assert_eq!(command["history"]["days"], 5, "days plus the spare margin");
    assert_eq!(
        result["totals"]["count"], 256,
        "totals cover every matching trade"
    );
    assert_eq!(result["totals"]["net"], 25.6);
    let shown = result["trades"].as_array().expect("trades").len();
    assert!(shown > 0 && shown < 256);
    assert_eq!(result["trades_omitted"], 256 - shown);
    assert_eq!(result["history_truncated"], true);
    assert!(
        result["note"]
            .as_str()
            .is_some_and(|note| note.contains("capped"))
    );
    assert_bounded(&result);
}

#[actix_web::test]
async fn closed_trades_validates_before_queueing_anything() {
    let fixture = fixture(true, Vec::new());
    for (arguments, expected) in [
        (json!("today"), "must be an object"),
        (json!({"since": "last tuesday"}), "RFC 3339"),
        (
            json!({"since": "today", "until": "yesterday"}),
            "since must be before until",
        ),
        (
            json!({"since": "2024-01-01T00:00:00Z"}),
            "within the last 365 days",
        ),
        (
            json!({"order": "buy"}),
            "unsupported closed_trades argument",
        ),
        (json!({"days": 0}), "days must be an integer"),
        (json!({"days": 400}), "days must be an integer"),
        (json!({"utc_offset_minutes": 900}), "utc_offset_minutes"),
        (json!({"symbol": "not a symbol"}), "symbol must be"),
    ] {
        let error = Tool::ClosedTrades
            .read(&fixture.state, &arguments)
            .await
            .expect_err("invalid arguments are refused");
        assert!(error.contains(expected), "{arguments}: {error}");
    }
    assert!(
        fixture.link.recent_commands(10).is_empty(),
        "nothing was queued for invalid arguments"
    );

    let unaligned = self::fixture(false, Vec::new());
    let error = Tool::ClosedTrades
        .read(&unaligned.state, &json!({"since": "today"}))
        .await
        .expect_err("no broker clock");
    assert!(error.starts_with("broker_clock_unknown"), "{error}");
    assert!(unaligned.link.recent_commands(10).is_empty());

    let error = Tool::ClosedTrades
        .read(&bare(), &json!({}))
        .await
        .expect_err("no broker");
    assert_eq!(error, "broker_unavailable");
}

#[actix_web::test]
async fn history_failures_are_bounded_reasons() {
    let fixture = fixture(true, Vec::new());
    let state = fixture.state.clone();
    let task =
        actix_web::rt::spawn(
            async move { Tool::Performance.read(&state, &json!({"days": 7})).await },
        );
    answer_next(&fixture.link, "order_history", Err("history unavailable")).await;
    let error = task.await.expect("joins").expect_err("terminal failure");
    assert_eq!(error, "order_history_failed: history unavailable");
    let error = Tool::Performance
        .read(&bare(), &json!({}))
        .await
        .expect_err("no broker");
    assert_eq!(error, "broker_unavailable");
    let error = Tool::Performance
        .read(&fixture.state, &json!({"days": "7"}))
        .await
        .expect_err("typed days");
    assert!(error.contains("days must be an integer"), "{error}");
}

#[actix_web::test]
async fn performance_returns_the_report_and_per_trade_detail() {
    let fixture = fixture(true, Vec::new());
    let (result, command) = with_history(
        &fixture,
        Tool::Performance,
        json!({"days": 7, "utc_offset_minutes": 120}),
        closes(),
        false,
    )
    .await;
    let result = result.expect("performance");
    assert_eq!(command["history"]["days"], 7);
    assert_eq!(result["days"], 7);
    assert_eq!(result["report"]["trades"], 3);
    assert_eq!(result["report"]["wins"], 2);
    assert_eq!(result["total"], 3);
    assert_eq!(result["truncated"], false);
    assert_eq!(
        tickets(&result, "trades"),
        vec![TICKET, 10_654_100, 10_654_000]
    );
    assert_eq!(result["trades"][0]["closed_utc"], "2025-03-12T06:46:06Z");
    assert_eq!(
        result["trades"][0]["closed_local"],
        "2025-03-12T08:46:06+02:00"
    );
    assert_bounded(&result);

    // Without a snapshot the report still answers, in labelled broker time.
    let unaligned = self::fixture(false, Vec::new());
    let (result, _) = with_history(&unaligned, Tool::Performance, json!({}), closes(), false).await;
    let result = result.expect("unaligned performance");
    assert_eq!(result["days"], 30);
    assert_eq!(result["broker_clock"]["known"], false);
    assert_eq!(result["trades"][0]["closed_broker"], "2025-03-12T08:46:06");
    assert!(result["trades"][0].get("closed_utc").is_none());
}

/// Records the lifecycle of one autopilot trade, plus noise, at fixed times.
fn journal(trail: &MemoryTrail) {
    let record = |time: &str, kind: AuditKind, payload: Value| {
        trail.record_at(
            OffsetDateTime::parse(time, &Rfc3339).expect("time"),
            AuditEvent::new(kind, payload),
        );
    };
    record(
        "2025-03-11T21:00:00Z",
        AuditKind::ProposalEvaluated,
        json!({"outcome": "queued", "origin": "autopilot", "symbol": "USDJPY", "side": "buy",
               "volume": 0.01, "stop_loss": 155.9, "take_profit": 156.8, "intent_id": "intent-1",
               "command_id": CMD_OPEN,
               "rationale": "Momentum resumed above the session VWAP with the calendar clear."}),
    );
    record(
        "2025-03-11T21:00:01Z",
        AuditKind::CommandQueued,
        json!({"command_id": CMD_OPEN, "kind": "open_order", "intent_id": "intent-1"}),
    );
    record(
        "2025-03-11T21:00:05Z",
        AuditKind::CommandCompleted,
        json!({"command_id": CMD_OPEN, "kind": "open_order",
               "result": {"executed": true, "retcode": 0, "ticket": TICKET}}),
    );
    for hour in 0..5 {
        record(
            &format!("2025-03-12T0{hour}:00:00Z"),
            AuditKind::ProposalEvaluated,
            json!({"outcome": "held", "origin": "autopilot_review", "symbol": "USDJPY",
                   "ticket": TICKET, "rationale": format!("Bracket intact, review {hour}.")}),
        );
    }
    record(
        "2025-03-12T05:00:00Z",
        AuditKind::ProposalEvaluated,
        json!({"outcome": "break_even", "origin": "autopilot", "symbol": "USDJPY",
               "ticket": TICKET, "command_id": CMD_MODIFY}),
    );
    record(
        "2025-03-12T05:00:03Z",
        AuditKind::CommandCompleted,
        json!({"command_id": CMD_MODIFY, "kind": "modify_order",
               "result": {"executed": true, "retcode": 0, "ticket": TICKET}}),
    );
    record(
        "2025-03-12T06:40:00Z",
        AuditKind::ProposalEvaluated,
        json!({"outcome": "profit_harvest_close", "origin": "profit_harvest", "symbol": "USDJPY",
               "ticket": TICKET, "high_net_profit": 2.1, "net_profit": 1.4, "command_id": CMD_CLOSE}),
    );
    record(
        "2025-03-12T06:46:09Z",
        AuditKind::CommandCompleted,
        json!({"command_id": CMD_CLOSE, "kind": "close_order",
               "result": {"executed": true, "retcode": 0, "ticket": TICKET}}),
    );
    record(
        "2025-03-12T06:46:10Z",
        AuditKind::PositionClosed,
        json!({"ticket": TICKET, "symbol": "USDJPY", "kind": "buy", "lots": 0.01,
               "price": 156.41, "profit": 1.36}),
    );
    record(
        "2025-03-12T07:00:00Z",
        AuditKind::ProposalEvaluated,
        json!({"outcome": "no_trade", "origin": "autopilot", "rationale": "r".repeat(2_000)}),
    );
    record(
        "2025-03-12T07:05:00Z",
        AuditKind::ProposalEvaluated,
        json!({"outcome": "rejected", "origin": "autopilot", "symbol": "EURUSD",
               "reason": "news_blackout"}),
    );
    record(
        "2025-03-12T07:06:00Z",
        AuditKind::BrokerSnapshot,
        json!({"orders": 0}),
    );
}

fn outcomes(result: &Value) -> Vec<String> {
    result["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| {
            event["outcome"]
                .as_str()
                .unwrap_or(event["kind"].as_str().unwrap_or(""))
                .to_owned()
        })
        .collect()
}

#[actix_web::test]
async fn decision_history_filters_the_durable_trail() {
    let (state, trail) = audit_only();
    journal(&trail);
    let read = |arguments: Value| {
        let state = state.clone();
        async move { Tool::DecisionHistory.read(&state, &arguments).await }
    };

    let all = read(json!({})).await.expect("defaults");
    assert_eq!(
        all["filters"]["kinds"],
        json!(["proposal_evaluated", "position_closed", "command_failed"])
    );
    assert_eq!(all["returned"], 11);
    assert_eq!(all["order"], "newest first");
    assert_eq!(all["retention_days"], 30);
    assert_eq!(
        outcomes(&all)[..3],
        ["rejected", "no_trade", "position_closed"]
    );
    assert_eq!(all["events"][0]["at"], "2025-03-12T07:05:00Z");
    assert_eq!(all["text_limit_chars"], 1_200);
    let clipped = all["events"][1]["rationale"].as_str().expect("rationale");
    assert_eq!(
        clipped.chars().count(),
        1_201,
        "1,200 characters plus the cut mark"
    );
    assert_bounded(&all);

    let eurusd = read(json!({"symbol": "eurusd"})).await.expect("symbol");
    assert_eq!(outcomes(&eurusd), ["rejected"]);
    assert_eq!(eurusd["events"][0]["reason"], "news_blackout");
    assert_eq!(eurusd["filters"]["symbol"], "EURUSD");

    let ticket = read(json!({"ticket": TICKET})).await.expect("ticket");
    assert_eq!(ticket["returned"], 8);
    let fills = read(json!({"ticket": TICKET, "kinds": ["command_completed"]}))
        .await
        .expect("fills");
    assert_eq!(
        fills["returned"], 3,
        "the open fill matches through result.ticket"
    );
    assert_eq!(fills["events"][2]["command_kind"], "open_order");
    assert_eq!(fills["events"][2]["result"]["ticket"], TICKET);

    let held = read(json!({"outcome": "held", "limit": 2}))
        .await
        .expect("held");
    assert_eq!(held["returned"], 2);
    assert_eq!(held["events"][0]["rationale"], "Bracket intact, review 4.");

    let window = read(json!({"since": "2025-03-12T05:00:00Z", "until": "2025-03-12T06:46:10Z"}))
        .await
        .expect("window");
    assert_eq!(outcomes(&window), ["profit_harvest_close", "break_even"]);
    assert_eq!(window["events"][0]["high_net_profit"], 2.1);
    assert_eq!(window["filters"]["until_utc"], "2025-03-12T06:46:10Z");

    let local = read(json!({"since": "today", "utc_offset_minutes": 120, "outcome": "queued"}))
        .await
        .expect("local today");
    assert_eq!(
        local["returned"], 0,
        "the 21:00 UTC entry is yesterday in UTC+2"
    );
    let yesterday = read(json!({"since": "yesterday", "until": "yesterday", "utc_offset_minutes": 120, "outcome": "queued"}))
        .await
        .expect("local yesterday");
    assert_eq!(yesterday["returned"], 1);
    assert_eq!(
        yesterday["events"][0]["at_local"],
        "2025-03-11T23:00:00+02:00"
    );
    assert_eq!(yesterday["events"][0]["intent_id"], "intent-1");

    let empty = read(json!({"symbol": "GBPUSD"})).await.expect("empty");
    assert_eq!(empty["events"], json!([]));
    assert_eq!(empty["returned"], 0);

    for (arguments, expected) in [
        (json!({"kinds": ["bogus"]}), "unknown kind `bogus`"),
        (
            json!({"limit": 51}),
            "limit must be an integer from 1 through 50",
        ),
        (
            json!({"outcome": "Bad Outcome"}),
            "invalid audit query `outcome`",
        ),
        (
            json!({"since": "today", "until": "yesterday"}),
            "since must be before until",
        ),
        (json!({"ticket": 0}), "ticket must be an integer"),
        (json!({"symbol": "no spaces"}), "symbol must be"),
        (
            json!({"extra": true}),
            "unsupported decision_history argument",
        ),
    ] {
        let error = read(arguments.clone()).await.expect_err("invalid filter");
        assert!(error.contains(expected), "{arguments}: {error}");
    }
    let error = Tool::DecisionHistory
        .read(&bare(), &json!({}))
        .await
        .expect_err("no trail");
    assert!(error.starts_with("audit_unavailable"), "{error}");
}

#[actix_web::test]
async fn decision_history_bounds_many_long_rationales() {
    let (state, trail) = audit_only();
    for index in 0..60 {
        trail
            .record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held", "symbol": "USDJPY", "ticket": index + 1,
                       "rationale": format!("{index} {}", "why ".repeat(600))}),
            ))
            .await
            .expect("record");
    }
    let result = Tool::DecisionHistory
        .read(&state, &json!({"limit": 50}))
        .await
        .expect("history");
    assert_eq!(result["returned"], 50);
    assert_eq!(result["text_limit_chars"], 240);
    let shown = result["events"].as_array().expect("events").len();
    assert_eq!(result["events_omitted"], 50 - shown);
    assert!(
        result["events"][0]["rationale"]
            .as_str()
            .is_some_and(|text| text.starts_with("59 "))
    );
    assert_bounded(&result);
}

#[actix_web::test]
async fn position_story_links_the_entry_rationale_adjustments_and_close() {
    let fixture = fixture(true, Vec::new());
    journal(&fixture.trail);
    let (result, command) = with_history(
        &fixture,
        Tool::PositionStory,
        json!({"ticket": TICKET, "utc_offset_minutes": 120}),
        closes(),
        false,
    )
    .await;
    let story = result.expect("story");
    assert_eq!(
        command["history"]["days"], 2,
        "look-back derived from the first event"
    );
    assert_eq!(story["status"], "closed");
    assert_eq!(story["symbol"], "USDJPY");
    assert_eq!(story["order"], "oldest first");
    let entry = &story["entry_decision"];
    assert_eq!(entry["outcome"], "queued");
    assert_eq!(entry["side"], "buy");
    assert_eq!(entry["stop_loss"], 155.9);
    assert_eq!(
        entry["rationale"],
        "Momentum resumed above the session VWAP with the calendar clear."
    );
    assert_eq!(entry["at_local"], "2025-03-11T23:00:00+02:00");
    assert_eq!(story["closed_trade"]["closed_utc"], "2025-03-12T06:46:06Z");
    assert_eq!(story["closed_trade"]["net"], 1.36);
    assert!(story.get("open_position").is_none());
    assert_eq!(story["gaps"], json!([]));
    assert_eq!(story["held_reviews"], json!({"total": 5, "shown": 3}));
    let timeline = story["timeline"].as_array().expect("timeline");
    let labels: Vec<&str> = timeline
        .iter()
        .map(|event| {
            event["outcome"]
                .as_str()
                .or(event["command_kind"].as_str())
                .unwrap_or("position_closed")
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "queued",
            "open_order",
            "open_order",
            "held",
            "held",
            "held",
            "break_even",
            "modify_order",
            "profit_harvest_close",
            "close_order",
            "position_closed"
        ]
    );
    assert_eq!(timeline[1]["kind"], "command_queued");
    assert_eq!(timeline[1]["intent_id"], "intent-1");
    assert_eq!(timeline[3]["rationale"], "Bracket intact, review 2.");
    assert_bounded(&story);
}

#[actix_web::test]
async fn position_story_reports_open_positions_and_evidence_gaps() {
    let fixture = fixture(
        true,
        vec![
            position(777, "2025-03-12T08:00:00Z"),
            position(888, "2025-03-12T09:00:00Z"),
        ],
    );
    let open = Tool::PositionStory
        .read(&fixture.state, &json!({"ticket": 777}))
        .await
        .expect("open story");
    assert_eq!(open["status"], "open");
    assert_eq!(open["book_fresh"], true);
    assert_eq!(open["book_age_secs"], 0);
    assert_eq!(open["open_position"]["opened_utc"], "2025-03-12T08:00:00Z");
    assert_eq!(open["open_position"]["held"], "2h 0m");
    assert_eq!(open["open_position"]["veyra"], true);
    assert_eq!(open["entry_decision"], Value::Null);
    assert!(
        open["gaps"][0]
            .as_str()
            .is_some_and(|gap| gap.contains("No open_order completion reporting ticket 777")),
        "{}",
        open["gaps"]
    );
    assert!(
        fixture.link.recent_commands(10).is_empty(),
        "an open ticket needs no history"
    );

    // Opened through a queued command with no autopilot decision attached.
    let manual = "8d6b8f4b-5e4a-4d8a-8a5a-ae3a5b0b1d43";
    fixture
        .trail
        .record(AuditEvent::new(
            AuditKind::CommandQueued,
            json!({"command_id": manual, "kind": "open_order", "intent_id": "manual-1"}),
        ))
        .await
        .expect("record");
    fixture
        .trail
        .record(AuditEvent::new(
            AuditKind::CommandCompleted,
            json!({"command_id": manual, "kind": "open_order", "result": {"executed": true, "ticket": 888}}),
        ))
        .await
        .expect("record");
    let story = Tool::PositionStory
        .read(&fixture.state, &json!({"ticket": 888}))
        .await
        .expect("manual story");
    let gap = story["gaps"][0].as_str().expect("gap");
    assert!(
        gap.contains("for intent manual-1") && gap.contains("no model rationale"),
        "{gap}"
    );
    assert_eq!(story["timeline"].as_array().map(Vec::len), Some(2));

    // Only the fill survives: the queue event is missing.
    let orphan = "9e7c9a5c-6f5b-4e9b-9b6b-bf4b6c1c2e54";
    fixture
        .trail
        .record(AuditEvent::new(
            AuditKind::CommandCompleted,
            json!({"command_id": orphan, "kind": "open_order", "result": {"executed": true, "ticket": 777}}),
        ))
        .await
        .expect("record");
    let story = Tool::PositionStory
        .read(&fixture.state, &json!({"ticket": 777}))
        .await
        .expect("orphan story");
    assert!(
        story["gaps"][0]
            .as_str()
            .is_some_and(|gap| gap.contains("neither its queued command"))
    );

    // Closed and not found in the terminal history either.
    let (result, _) = with_history(
        &fixture,
        Tool::PositionStory,
        json!({"ticket": 999, "days": 9}),
        Vec::new(),
        true,
    )
    .await;
    let missing = result.expect("missing story");
    assert_eq!(missing["status"], "not_found");
    assert!(
        missing["gaps"][0]
            .as_str()
            .is_some_and(|gap| gap.contains("last 9 days") && gap.contains("capped"))
    );

    // No broker: the trail still answers and the gap is explicit.
    let (state, trail) = audit_only();
    journal(&trail);
    let story = Tool::PositionStory
        .read(&state, &json!({"ticket": TICKET}))
        .await
        .expect("story without broker");
    assert_eq!(story["status"], "unknown");
    assert_eq!(story["history_error"], "broker_unavailable");
    assert_eq!(story["broker_clock"]["known"], false);
    assert_eq!(story["entry_decision"]["outcome"], "queued");

    for (arguments, expected) in [
        (json!({}), "ticket is required"),
        (json!({"ticket": -1}), "ticket must be an integer"),
        (json!({"ticket": 1, "days": 0}), "days must be an integer"),
    ] {
        let error = Tool::PositionStory
            .read(&fixture.state, &arguments)
            .await
            .expect_err("invalid story request");
        assert!(error.contains(expected), "{arguments}: {error}");
    }
    let error = Tool::PositionStory
        .read(&bare(), &json!({"ticket": 1}))
        .await
        .expect_err("no trail");
    assert!(error.starts_with("audit_unavailable"), "{error}");
}

#[actix_web::test]
async fn recent_commands_show_outcomes_and_who_queued_them() {
    let fixture = fixture(true, Vec::new());
    let modify = fixture.link.enqueue_modify(ModifyOrderRequest::new(
        TICKET,
        ORDER_MAGIC,
        Some(156.2),
        None,
    ));
    let close = fixture
        .link
        .enqueue_close(CloseOrderRequest::new(TICKET, ORDER_MAGIC));
    let snapshot = fixture.link.enqueue(CommandKind::AccountSnapshot);
    fixture
        .trail
        .record(AuditEvent::new(
            AuditKind::CommandQueued,
            json!({"command_id": modify.to_string(), "kind": "modify_order", "ticket": TICKET}),
        ))
        .await
        .expect("record");
    fixture
        .trail
        .record(AuditEvent::new(
            AuditKind::ProposalEvaluated,
            json!({"outcome": "break_even", "origin": "autopilot", "symbol": "USDJPY",
                   "ticket": TICKET, "command_id": modify.to_string()}),
        ))
        .await
        .expect("record");
    answer_next(
        &fixture.link,
        "modify_order",
        Ok(json!({"executed": true, "retcode": 0, "comment": "modified", "ticket": TICKET, "price": 156.2})),
    )
    .await;
    answer_next(&fixture.link, "close_order", Err("terminal busy")).await;

    let result = Tool::RecentCommands
        .read(&fixture.state, &json!({"utc_offset_minutes": 120}))
        .await
        .expect("commands");
    assert_eq!(result["audit_context"], "joined");
    assert_eq!(result["routine_omitted"], 1);
    let commands = result["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["id"], close.to_string());
    assert_eq!(commands[0]["status"], "failed");
    assert_eq!(commands[0]["reason"], "terminal busy");
    assert_eq!(commands[1]["kind"], "modify_order");
    assert_eq!(commands[1]["detail"]["executed"], true);
    assert_eq!(commands[1]["detail"]["price"], 156.2);
    assert_eq!(commands[1]["ticket"], TICKET);
    assert!(commands[1]["queued_at"].is_string());
    assert!(commands[1]["queued_local"].is_string());
    assert_eq!(commands[1]["decision"]["outcome"], "break_even");
    assert_bounded(&result);

    let everything = Tool::RecentCommands
        .read(&fixture.state, &json!({"include_routine": true}))
        .await
        .expect("routine included");
    assert_eq!(everything["commands"][0]["id"], snapshot.to_string());
    assert_eq!(everything["commands"][0]["status"], "pending");
    assert!(everything.get("routine_omitted").is_none());

    let only = Tool::RecentCommands
        .read(
            &fixture.state,
            &json!({"kind": "account_snapshot", "limit": 1}),
        )
        .await
        .expect("kind filter");
    assert_eq!(only["kind"], "account_snapshot");
    assert_eq!(only["commands"].as_array().map(Vec::len), Some(1));

    for (arguments, expected) in [
        (json!({"kind": "launch"}), "unknown command kind `launch`"),
        (json!({"limit": 0}), "limit must be an integer"),
        (
            json!({"include_routine": "yes"}),
            "include_routine must be true or false",
        ),
    ] {
        let error = Tool::RecentCommands
            .read(&fixture.state, &arguments)
            .await
            .expect_err("invalid");
        assert!(error.contains(expected), "{arguments}: {error}");
    }
    let error = Tool::RecentCommands
        .read(&bare(), &json!({}))
        .await
        .expect_err("no broker");
    assert_eq!(error, "broker_unavailable");
}

#[actix_web::test]
async fn completed_command_details_stay_bounded_and_balance_free() {
    use crate::broker::{
        CandlePayload, ClosedTradePayload, CommandPayload, OrderCheckPayload,
        OrderExecutionPayload, OrderHistoryPayload, RatesPayload,
    };
    let snapshot = AccountSnapshotPayload {
        balance: 20.57,
        equity: 21.0,
        free_margin: 19.0,
        orders: 1,
        lots: 0.01,
        positions: vec![position(1, "2025-03-12T08:00:00Z")],
        positions_truncated: false,
        server_time: now(),
        leverage: 100,
        margin_level: 0.0,
    };
    let detail = super::journal::command_detail(&CommandPayload::AccountSnapshot(snapshot));
    assert_eq!(detail["tickets"], json!([1]));
    assert!(
        detail.get("balance").is_none(),
        "balances stay out of command detail"
    );
    assert_eq!(
        super::journal::command_detail(&CommandPayload::Ping),
        json!({})
    );
    let check = super::journal::command_detail(&CommandPayload::OrderCheck(OrderCheckPayload {
        passed: false,
        retcode: 134,
        comment: "c".repeat(300),
        margin: 3.2,
    }));
    assert_eq!(check["retcode"], 134);
    assert_eq!(
        check["comment"].as_str().map(|text| text.chars().count()),
        Some(161)
    );
    let open = super::journal::command_detail(&CommandPayload::OpenOrder(OrderExecutionPayload {
        executed: true,
        retcode: 0,
        comment: String::new(),
        ticket: 5,
        price: 1.1,
    }));
    assert_eq!(open["ticket"], 5);
    let rates = super::journal::command_detail(&CommandPayload::Rates(RatesPayload {
        symbol: "USDJPY".to_owned(),
        timeframe_minutes: 15,
        candles: vec![CandlePayload {
            time: 1,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1,
        }],
    }));
    assert_eq!(rates["candles"], 1);
    let history =
        super::journal::command_detail(&CommandPayload::OrderHistory(OrderHistoryPayload {
            orders: vec![ClosedTradePayload {
                ticket: 9,
                symbol: "USDJPY".to_owned(),
                kind: PositionKind::Buy,
                lots: 0.01,
                open_price: 1.0,
                close_price: 1.1,
                open_time: 1,
                close_time: 2,
                profit: 0.1,
                swap: 0.0,
                commission: 0.0,
                magic: ORDER_MAGIC,
            }],
            total: 1,
            truncated: false,
        }));
    assert_eq!(history["tickets"], json!([9]));
}

#[actix_web::test]
async fn positions_carry_utc_open_times_and_fit_the_ceiling() {
    let book: Vec<PositionPayload> = (1..=64)
        .map(|ticket| position(ticket, "2025-03-12T04:30:00Z"))
        .collect();
    let fixture = fixture(true, book);
    let result = Tool::Positions
        .read(&fixture.state, &json!({"utc_offset_minutes": 120}))
        .await
        .expect("positions");
    assert_eq!(result["fresh"], true);
    assert_eq!(result["broker_clock"]["utc_offset"], "+02:00");
    let first = &result["positions"][0];
    assert_eq!(first["opened_utc"], "2025-03-12T04:30:00Z");
    assert_eq!(first["opened_local"], "2025-03-12T06:30:00+02:00");
    assert_eq!(first["held"], "5h 30m");
    assert_eq!(first["side"], "Long");
    assert_eq!(first["net"], 0.82);
    let shown = result["positions"].as_array().expect("positions").len();
    assert_eq!(result["positions_omitted"], 64 - shown);
    assert_bounded(&result);

    let empty = self::fixture(false, Vec::new());
    let result = Tool::Positions
        .read(&empty.state, &json!({}))
        .await
        .expect("no snapshot");
    assert_eq!(result["positions"], Value::Null);
    assert!(result["note"].is_string());
    assert!(Tool::Positions.read(&bare(), &json!({})).await.is_err());
    assert!(
        Tool::Positions
            .read(&empty.state, &json!({"verbose": true}))
            .await
            .is_err()
    );
}

#[actix_web::test]
async fn activity_and_account_answer_from_retained_state() {
    let fixture = fixture(true, vec![position(777, "2025-03-12T08:00:00Z")]);
    journal(&fixture.trail);
    let activity = Tool::Activity
        .read(&fixture.state, &json!({}))
        .await
        .expect("activity");
    assert_eq!(activity["limited_to"], 35);
    assert_eq!(activity["events"][0]["outcome"], "rejected");
    assert_bounded(&activity);
    assert!(Tool::Activity.read(&bare(), &json!({})).await.is_err());

    let account = Tool::Account
        .read(&fixture.state, &json!({}))
        .await
        .expect("account");
    assert_eq!(account["balance"], 20.57);
    assert_eq!(account["connected"], true);
    assert!(
        Tool::Account
            .read(&fixture.state, &json!({"x": 1}))
            .await
            .is_err()
    );
    let status = Tool::ModelStatus
        .read(&fixture.state, &json!({}))
        .await
        .expect("model status");
    assert_eq!(status["configured"], false);
    let sessions = Tool::MarketSessions
        .read(&fixture.state, &json!({}))
        .await
        .expect("sessions");
    assert!(sessions["next_at_utc"].is_string());
}

#[actix_web::test]
async fn balance_history_collapses_unchanged_readings() {
    let fixture = fixture(true, Vec::new());
    let start_ms = (now() - 3_600) * 1_000;
    for minute in 0..300_i64 {
        let balance = if minute < 100 {
            20.0
        } else if minute < 200 {
            21.5
        } else {
            19.25
        };
        fixture
            .trail
            .record(AuditEvent::new(
                AuditKind::BalanceObserved,
                json!({"login": 94168, "server": "IFCMarkets-Real", "balance": balance,
                       "atMs": start_ms + minute * 12_000}),
            ))
            .await
            .expect("record");
    }
    let result = Tool::BalanceHistory
        .read(&fixture.state, &json!({"days": 1}))
        .await
        .expect("balance history");
    assert_eq!(result["observations"], 300);
    let balances: Vec<f64> = result["points"]
        .as_array()
        .expect("points")
        .iter()
        .map(|point| point["balance"].as_f64().expect("balance"))
        .collect();
    assert_eq!(
        balances,
        vec![20.0, 21.5, 19.25, 19.25],
        "changes plus the last reading"
    );
    assert_eq!(result["points"][0]["at"], "2025-03-12T09:00:00Z");
    assert_bounded(&result);

    let many: Vec<crate::balance::BalancePoint> = (0..1_000)
        .map(|index| crate::balance::BalancePoint {
            at_ms: index,
            balance: f64::from(u32::try_from(index).expect("small")),
        })
        .collect();
    let sampled = super::balance_points(&many);
    assert_eq!(sampled.len(), super::MAX_BALANCE_POINTS);
    assert_eq!(sampled.first().map(|point| point.at_ms), Some(0));
    assert_eq!(sampled.last().map(|point| point.at_ms), Some(999));

    let waiting = Tool::BalanceHistory
        .read(&audit_only().0, &json!({}))
        .await
        .expect_err("no broker");
    assert_eq!(waiting, "broker_unavailable");
    assert!(
        Tool::BalanceHistory
            .read(&fixture.state, &json!({"days": 0}))
            .await
            .is_err()
    );
}

#[derive(Debug)]
struct StubMarket;

#[async_trait]
impl MarketFeed for StubMarket {
    fn provider(&self) -> MarketProvider {
        MarketProvider::Ea
    }

    async fn candles(&self, request: CandleRequest) -> Result<CandleSeries, MarketError> {
        Ok(CandleSeries::from_validated(
            request.symbol().clone(),
            request.timeframe(),
            vec![Candle::from_validated(
                broker("2025-03-12T09:45:00Z"),
                156.1,
                156.3,
                156.0,
                156.2,
                42,
            )],
        ))
    }

    async fn symbol_spec(
        &self,
        symbol: &Symbol,
    ) -> Result<crate::broker::SymbolSpecPayload, MarketError> {
        Err(MarketError::Unavailable {
            reason: format!("no spec for {}", symbol.as_str()),
        })
    }
}

/// Market stub that answers the venue contract.
#[derive(Debug)]
struct SpecMarket;

#[async_trait]
impl MarketFeed for SpecMarket {
    fn provider(&self) -> MarketProvider {
        MarketProvider::Ea
    }

    async fn candles(&self, _request: CandleRequest) -> Result<CandleSeries, MarketError> {
        Err(MarketError::Unavailable {
            reason: "no candles".to_owned(),
        })
    }

    async fn symbol_spec(
        &self,
        symbol: &Symbol,
    ) -> Result<crate::broker::SymbolSpecPayload, MarketError> {
        Ok(spec(symbol.as_str()))
    }
}

fn spec(symbol: &str) -> crate::broker::SymbolSpecPayload {
    crate::broker::SymbolSpecPayload {
        currency_base: None,
        currency_profit: None,
        sessions: Vec::new(),
        symbol: symbol.to_owned(),
        digits: 3,
        point: 0.001,
        bid: 156.2,
        ask: 156.212,
        spread_points: 12,
        stop_level_points: 5,
        freeze_level_points: 0,
        lot_min: 0.01,
        lot_max: 100.0,
        lot_step: 0.01,
        tick_value: 0.64,
        tick_size: 0.001,
        margin_required: 3.29,
        swap_long: -0.72,
        swap_short: -0.31,
        swap_type: 0,
        trade_allowed: true,
    }
}

/// Trail that is configured but cannot be read.
#[derive(Debug)]
struct UnreadableTrail;

#[async_trait]
impl AuditTrail for UnreadableTrail {
    fn provider(&self) -> crate::audit::AuditProvider {
        crate::audit::AuditProvider::Postgres
    }

    async fn record(&self, _event: AuditEvent) -> Result<(), crate::audit::AuditError> {
        Ok(())
    }

    async fn recent(
        &self,
        _limit: u32,
    ) -> Result<Vec<crate::audit::AuditRow>, crate::audit::AuditError> {
        Err(crate::audit::AuditError::Storage {
            reason: "database down".to_owned(),
        })
    }

    async fn prune(&self, _keep_days: u32) -> Result<u64, crate::audit::AuditError> {
        Ok(0)
    }
}

#[actix_web::test]
async fn unreadable_trails_and_unknown_clocks_are_reported_not_hidden() {
    let fixture = fixture(true, Vec::new());
    let broken = fixture
        .state
        .clone()
        .with_audit(Some(AuditRuntime::new(Arc::new(UnreadableTrail))));
    for (tool, arguments) in [
        (Tool::DecisionHistory, json!({})),
        (Tool::PositionStory, json!({"ticket": TICKET})),
        (Tool::Activity, json!({})),
    ] {
        let error = tool
            .read(&broken, &arguments)
            .await
            .expect_err("unreadable trail");
        assert_eq!(
            error,
            "audit_unavailable: the durable audit trail could not be read"
        );
    }
    fixture
        .link
        .enqueue_close(CloseOrderRequest::new(TICKET, ORDER_MAGIC));
    let commands = Tool::RecentCommands
        .read(&broken, &json!({}))
        .await
        .expect("commands without context");
    assert_eq!(commands["audit_context"], "unavailable");
    assert_eq!(commands["commands"][0]["status"], "pending");

    // A server clock 20 hours off UTC is refused, and times stay labelled.
    let skewed = fixture_with(
        Some(now() + 20 * 3_600),
        vec![position(777, "2025-03-12T08:00:00Z")],
    );
    let positions = Tool::Positions
        .read(&skewed.state, &json!({}))
        .await
        .expect("positions");
    assert_eq!(positions["broker_clock"]["known"], false);
    assert!(
        positions["broker_clock"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("broker_clock_implausible"))
    );
    assert_eq!(
        positions["positions"][0]["opened_broker"],
        "2025-03-12T10:00:00"
    );
    assert!(positions["positions"][0].get("opened_utc").is_none());
    let error = Tool::ClosedTrades
        .read(&skewed.state, &json!({"since": "today"}))
        .await
        .expect_err("implausible clock");
    assert!(error.starts_with("broker_clock_implausible"), "{error}");
}

#[actix_web::test]
async fn balance_history_reports_missing_account_and_trail() {
    let settings = BrokerSettings::from_source(|name| match name {
        "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
        "VEYRA_EA_TOKEN" => Ok(TOKEN.to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("settings")
    .expect("configured");
    let silent = AppState::new(
        config(),
        Some(BrokerRuntime::from_settings(settings).expect("runtime")),
        None,
        RiskGate::new(RiskPolicy::default()),
    );
    let waiting = Tool::BalanceHistory
        .read(&silent, &json!({}))
        .await
        .expect("waiting");
    assert_eq!(waiting["status"], "waiting_for_account");
    let fixture = fixture(true, Vec::new());
    let untracked = fixture.state.clone().with_audit(None);
    let disabled = Tool::BalanceHistory
        .read(&untracked, &json!({"days": 2}))
        .await
        .expect("disabled");
    assert_eq!(disabled["status"], "disabled");
    assert_eq!(disabled["days"], 2);
}

#[actix_web::test]
async fn registered_tools_execute_through_the_read_only_contract() {
    let fixture = fixture(true, Vec::new());
    let state = fixture
        .state
        .clone()
        .with_market(Some(MarketRuntime::from_feed(Arc::new(SpecMarket))));
    let spec = Tool::MarketSpec
        .read(&state, &json!({}))
        .await
        .expect_err("no open position to name a symbol");
    assert_eq!(spec, "no_open_position_symbol");
    let with_book = self::fixture(true, vec![position(777, "2025-03-12T08:00:00Z")]);
    let state = with_book
        .state
        .clone()
        .with_market(Some(MarketRuntime::from_feed(Arc::new(SpecMarket))));
    let tools = super::read_only_tools(&state);
    let market_spec = tools
        .iter()
        .find(|tool| tool.definition().name == "market_spec")
        .expect("registered");
    let result = market_spec.execute(json!({})).await.expect("spec");
    assert_eq!(result["symbol"], "USDJPY");
    assert_eq!(result["spec"]["spreadPoints"], 12);
    let candles = tools
        .iter()
        .find(|tool| tool.definition().name == "market_candles")
        .expect("registered");
    assert_eq!(
        candles.execute(json!({})).await.expect_err("no candles"),
        "market_candles_unavailable"
    );
    let detail =
        super::journal::command_detail(&crate::broker::CommandPayload::SymbolSpec(spec_payload()));
    assert_eq!(detail["spread_points"], 12);
    assert_eq!(detail["trade_allowed"], true);
}

fn spec_payload() -> crate::broker::SymbolSpecPayload {
    spec("USDJPY")
}

#[derive(Debug)]
struct StubCalendar;

#[async_trait]
impl EventCalendar for StubCalendar {
    fn provider(&self) -> CalendarProvider {
        CalendarProvider::Forexfactory
    }

    async fn events(&self, from: i64, _until: i64) -> Result<Vec<CalendarEvent>, CalendarError> {
        Ok(vec![
            CalendarEvent::new("US CPI", "USD", Impact::High, from + 3_600).expect("event"),
        ])
    }
}

#[actix_web::test]
async fn market_and_calendar_tools_convert_and_bound_their_times() {
    let fixture = fixture(true, vec![position(777, "2025-03-12T08:00:00Z")]);
    let state = fixture
        .state
        .clone()
        .with_market(Some(MarketRuntime::from_feed(Arc::new(StubMarket))))
        .with_calendar(Some(CalendarRuntime::from_feed(Arc::new(StubCalendar))));
    let candles = Tool::MarketCandles
        .read(&state, &json!({}))
        .await
        .expect("candles");
    assert_eq!(candles["symbol"], "USDJPY");
    assert_eq!(candles["candles"][0]["time_utc"], "2025-03-12T09:45:00Z");
    let error = Tool::MarketSpec
        .read(&state, &json!({}))
        .await
        .expect_err("spec failure");
    assert_eq!(error, "market_spec_unavailable");
    assert!(
        Tool::MarketSpec
            .read(&fixture.state, &json!({}))
            .await
            .is_err()
    );
    let events = Tool::Calendar
        .read(&state, &json!({"hours": 12}))
        .await
        .expect("calendar");
    assert_eq!(events["hours"], 12);
    assert_eq!(events["events"][0]["time_utc"], "2025-03-12T11:00:00Z");
    let error = Tool::Calendar
        .read(&state, &json!({"hours": 0}))
        .await
        .expect_err("hours");
    assert_eq!(error, "hours must be an integer from 1 through 168");
    assert!(
        Tool::Calendar
            .read(&fixture.state, &json!({}))
            .await
            .is_err()
    );
}
