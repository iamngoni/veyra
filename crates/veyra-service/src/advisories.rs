//! Plain-language notices for the console banner: `GET /advisories`.
//!
//! Each advisory answers "why isn't Veyra trading right now, and until when?"
//! from live state: the operator switches, the broker link, the drawdown
//! breakers, the autopilot's recent rounds, and the market windows (the FX
//! week, the nightly rollover pause, and each index's own trading hours).
//!
//! Read-only: nothing here changes state or queues an order. It may ask the
//! terminal for the contract of an instrument whose hours only the terminal
//! knows (index CFDs), which is a routine read, bounded by a short timeout so
//! a stale link cannot stall the banner.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use actix_web::{HttpResponse, get, web};
use serde::Serialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::audit::AuditKind;
use crate::broker::Symbol;
use crate::risk::window::{self, InstrumentSession, WindowBlock};

/// How recent an unavailable autopilot round must be to be reported.
const SKIPPED_ROUND_WINDOW_MS: u64 = 10 * 60 * 1_000;

/// Longest a contract lookup may take before the banner goes without it.
const SPEC_TIMEOUT: Duration = Duration::from_secs(3);

/// Furthest ahead a reopening is searched for (just over a week).
const SEARCH_MINUTES: i64 = 8 * 24 * 60;

/// How urgent an advisory is, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Trading is stopped by something the operator must act on.
    Critical,
    /// Something is degraded or needs a look.
    Warning,
    /// Expected pauses, such as a closed market.
    Info,
}

/// One notice for the banner.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Advisory {
    /// Stable identifier.
    pub id: &'static str,
    /// Urgency.
    pub severity: Severity,
    /// Short headline.
    pub title: String,
    /// Optional plain-language explanation.
    pub detail: Option<String>,
    /// When the condition is expected to end, UTC milliseconds.
    pub until_ms: Option<i64>,
}

fn advisory(
    id: &'static str,
    severity: Severity,
    title: impl Into<String>,
    detail: Option<String>,
    until_ms: Option<i64>,
) -> Advisory {
    Advisory {
        id,
        severity,
        title: title.into(),
        detail,
        until_ms,
    }
}

fn unix_secs(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The first whole minute after `start` (UTC seconds) where `open` holds,
/// searching up to [`SEARCH_MINUTES`] ahead.
pub fn next_open(start: i64, open: impl Fn(i64) -> bool) -> Option<i64> {
    let first = start - start.rem_euclid(60) + 60;
    (0..SEARCH_MINUTES)
        .map(|minute| first + minute * 60)
        .find(|at| open(*at))
}

/// Joins names as "A", "A and B", or "A, B and C".
fn list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Market-window notices: the FX week, the nightly pause, the operator's
/// session hours.
fn window_advisories(
    now: SystemTime,
    session: Option<crate::risk::SessionWindow>,
    weekend_symbols: &[Symbol],
) -> Vec<Advisory> {
    let Some(block) = window::entry_block(now, session) else {
        return Vec::new();
    };
    let reopen = next_open(unix_secs(now), |at| {
        window::entry_block(UNIX_EPOCH + Duration::from_secs(at.max(0) as u64), session).is_none()
    })
    .map(|at| at * 1_000);
    let still_trading = (!weekend_symbols.is_empty()).then(|| {
        let names: Vec<String> = weekend_symbols
            .iter()
            .map(|symbol| symbol.as_str().to_owned())
            .collect();
        format!("{} still trade.", list(&names))
    });
    let item = match block {
        WindowBlock::WeekendOpen => advisory(
            "market_closed",
            Severity::Info,
            "FX, gold and indices are closed for the weekend",
            still_trading,
            reopen,
        ),
        WindowBlock::WeekendApproach => advisory(
            "entry_cutoff",
            Severity::Info,
            "No new trades until the week reopens",
            Some(match still_trading {
                Some(trading) => format!("Friday cutoff. Open trades are still managed. {trading}"),
                None => "Friday cutoff. Open trades are still managed.".to_owned(),
            }),
            reopen,
        ),
        WindowBlock::RolloverBlackout => advisory(
            "rollover_pause",
            Severity::Info,
            "Nightly pause",
            Some("Spreads widen around the daily rollover, so no new trades.".to_owned()),
            reopen,
        ),
        WindowBlock::SessionClosed => advisory(
            "session_closed",
            Severity::Info,
            "Outside your trading hours",
            Some("Set in Risk → session window.".to_owned()),
            reopen,
        ),
    };
    vec![item]
}

/// Instruments with their own hours (index CFDs) that are closed now while
/// the FX week is open, with the earliest reopening.
async fn instrument_advisory(state: &AppState, now: SystemTime) -> Option<Advisory> {
    let settings = state.autopilot()?;
    let market = state.market()?;
    let policy = state.risk().policy();
    let offset = crate::broker_clock::BrokerClock::from_state(state)
        .ok()
        .map(crate::broker_clock::BrokerClock::offset_secs);
    let utc = unix_secs(now);
    let mut closed: Vec<(String, Option<i64>)> = Vec::new();
    for symbol in settings.symbols() {
        if crate::risk::valuation::supports_static_valuation(symbol)
            || policy.allows_weekend(symbol)
        {
            continue;
        }
        let Ok(Ok(spec)) =
            actix_web::rt::time::timeout(SPEC_TIMEOUT, market.feed().symbol_spec(symbol)).await
        else {
            continue;
        };
        if window::entry_session(&spec, symbol, false, utc, offset) != InstrumentSession::Closed {
            continue;
        }
        let reopen = next_open(utc, |at| {
            window::instrument_session(&spec.sessions, at, offset) == InstrumentSession::Open
        });
        closed.push((symbol.as_str().to_owned(), reopen));
    }
    closed_instruments_item(&closed)
}

/// Breaker notices from the latest equity, with what it takes to resume.
fn breaker_advisories(state: &AppState, now: SystemTime) -> Vec<Advisory> {
    let Some(account) = state
        .broker()
        .and_then(|broker| broker.link().last_account())
    else {
        return Vec::new();
    };
    let policy = state.risk().policy();
    let drawdowns = state.equity_guard().observe(account.equity, now);
    let peak = state.equity_guard().state_snapshot()["peakEquity"].as_f64();
    breaker_items(
        (
            policy.max_daily_loss_percent(),
            policy.max_peak_drawdown_percent(),
        ),
        (drawdowns.day_percent, drawdowns.peak_percent),
        peak,
        unix_secs(now),
    )
}

/// The breaker notices for `drawdowns` (day, peak percent) against `limits`,
/// given the recorded peak equity and the time (UTC seconds).
fn breaker_items(
    limits: (f64, f64),
    drawdowns: (f64, f64),
    peak: Option<f64>,
    now: i64,
) -> Vec<Advisory> {
    let (daily, peak_limit) = limits;
    let (day_percent, peak_percent) = drawdowns;
    let mut out = Vec::new();
    if daily > 0.0 && day_percent >= daily {
        let next_day = (now.div_euclid(86_400) + 1) * 86_400;
        out.push(advisory(
            "breaker_daily",
            Severity::Critical,
            format!("New trades paused: down {day_percent:.1}% today"),
            Some(format!(
                "Daily loss limit is {daily:.0}%. Open trades are still managed."
            )),
            Some(next_day * 1_000),
        ));
    }
    if peak_limit > 0.0 && peak_percent >= peak_limit {
        let resume = peak.map(|peak| peak * (1.0 - peak_limit / 100.0));
        out.push(advisory(
            "breaker_peak",
            Severity::Critical,
            format!("New trades paused: {peak_percent:.1}% below the account's peak"),
            Some(match resume {
                Some(level) => format!(
                    "Limit is {peak_limit:.0}%. Resumes when equity is back above {level:.2}, or when the peak is reset."
                ),
                None => format!("Limit is {peak_limit:.0}%."),
            }),
            None,
        ));
    }
    out
}

/// Turns a recorded failure reason into plain words.
fn plain_reason(reason: &str) -> String {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("calendar") {
        "The news calendar could not be read.".to_owned()
    } else if lower.contains("judgement") || lower.contains("jev") {
        "The market-reading service did not answer.".to_owned()
    } else if lower.contains("model") {
        "The AI model did not answer.".to_owned()
    } else if lower.contains("market data") || lower.contains("candles") || lower.contains("rates")
    {
        "Market data from MT4 was not available.".to_owned()
    } else {
        crate::text::clip(reason, 140)
    }
}

/// The latest autopilot round, when it was skipped recently.
async fn skipped_round(state: &AppState, now: SystemTime) -> Option<Advisory> {
    let audit = state.audit()?;
    let latest = audit.feed_latest();
    let events = audit
        .feed_after(latest.saturating_sub(256), 256, Duration::ZERO)
        .await;
    let round = events
        .iter()
        .rev()
        .find(|event| event.kind == AuditKind::ProposalEvaluated)?;
    let now_ms = u64::try_from(unix_secs(now)).unwrap_or(0) * 1_000;
    if round.payload.get("outcome").and_then(Value::as_str) != Some("unavailable")
        || now_ms.saturating_sub(round.at_ms) > SKIPPED_ROUND_WINDOW_MS
    {
        return None;
    }
    let reason = round
        .payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(advisory(
        "autopilot_skipped",
        Severity::Warning,
        "The autopilot skipped its last round",
        Some(plain_reason(reason)),
        None,
    ))
}

/// The broker-link notice: MT4 not reporting outranks a disarmed EA.
fn link_item(reporting: bool, live_orders: bool) -> Option<Advisory> {
    if !reporting {
        return Some(advisory(
            "broker_stale",
            Severity::Critical,
            "MT4 is not reporting",
            Some("No trades or position management until the terminal reconnects.".to_owned()),
            None,
        ));
    }
    (!live_orders).then(|| {
        advisory(
            "ea_disarmed",
            Severity::Warning,
            "The EA is disarmed",
            Some("Orders are dry runs until live orders are allowed in MT4.".to_owned()),
            None,
        )
    })
}

/// The notice for instruments closed on their own hours, with the earliest
/// reopening (UTC seconds) among them.
fn closed_instruments_item(closed: &[(String, Option<i64>)]) -> Option<Advisory> {
    if closed.is_empty() {
        return None;
    }
    let names: Vec<String> = closed.iter().map(|(name, _)| name.clone()).collect();
    let reopen = closed.iter().filter_map(|(_, at)| *at).min();
    Some(advisory(
        "instrument_closed",
        Severity::Info,
        format!(
            "{} {} closed",
            list(&names),
            if names.len() == 1 { "is" } else { "are" }
        ),
        None,
        reopen.map(|at| at * 1_000),
    ))
}

/// Every advisory in force now, most severe first.
pub async fn collect(state: &AppState) -> Vec<Advisory> {
    let now = state.now();
    let policy = state.risk().policy();
    let mut items = Vec::new();

    if policy.kill_switch() {
        items.push(advisory(
            "kill_switch",
            Severity::Critical,
            "Kill switch is on",
            Some("No new trades until it is turned off in Risk.".to_owned()),
            None,
        ));
    }
    if !state.trading_enabled() {
        items.push(advisory(
            "trading_disabled",
            Severity::Critical,
            "Trading is switched off",
            Some("Turn on Trading enabled in Settings to resume.".to_owned()),
            None,
        ));
    }
    match state.broker() {
        None => items.push(advisory(
            "broker_missing",
            Severity::Critical,
            "No broker connected",
            None,
            None,
        )),
        Some(broker) => {
            let report = broker.link().report().await;
            let snapshot = report.snapshot.as_ref();
            items.extend(link_item(
                report.fresh && snapshot.is_some_and(|snapshot| snapshot.connected()),
                snapshot.is_some_and(|snapshot| snapshot.live_orders()),
            ));
        }
    }
    items.extend(breaker_advisories(state, now));

    let failures = state.decision_health().consecutive_failures();
    if failures >= 3 {
        let reason = state
            .decision_health()
            .last_failure()
            .map(|(reason, _)| plain_reason(&reason));
        items.push(advisory(
            "model_trouble",
            Severity::Warning,
            format!("The autopilot failed {failures} rounds in a row"),
            reason,
            None,
        ));
    } else if let Some(skipped) = skipped_round(state, now).await {
        items.push(skipped);
    }

    let window_items = window_advisories(now, policy.session(), policy.weekend_symbols());
    let fx_week_open = window_items
        .iter()
        .all(|item| item.id == "rollover_pause" || item.id == "session_closed");
    items.extend(window_items);
    if fx_week_open && let Some(item) = instrument_advisory(state, now).await {
        items.push(item);
    }

    items.sort_by_key(|item| item.severity);
    items
}

#[get("/advisories")]
/// Plain-language notices for the console banner, most severe first.
pub async fn advisories(state: web::Data<AppState>) -> HttpResponse {
    let items = collect(&state).await;
    let generated = unix_secs(state.now()) * 1_000;
    HttpResponse::Ok().json(json!({ "items": items, "generatedAtMs": generated }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    }

    // Saturday 2026-09-26 12:00 UTC.
    const SATURDAY_NOON: i64 = 1_790_424_000;

    #[test]
    fn next_open_finds_the_first_open_minute() {
        assert_eq!(next_open(0, |at| at >= 3_600), Some(3_600));
        assert_eq!(next_open(59, |_| true), Some(60), "whole minutes after now");
        assert_eq!(next_open(0, |_| false), None);
    }

    #[test]
    fn names_join_naturally() {
        assert_eq!(list(&[]), "");
        assert_eq!(list(&["BTCUSD".to_owned()]), "BTCUSD");
        assert_eq!(
            list(&["A".to_owned(), "B".to_owned(), "C".to_owned()]),
            "A, B and C"
        );
    }

    #[test]
    fn the_weekend_reports_when_entries_reopen_and_what_still_trades() {
        let crypto = vec![Symbol::parse("BTCUSD").expect("symbol")];
        let items = window_advisories(at(SATURDAY_NOON), None, &crypto);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "market_closed");
        assert_eq!(items[0].detail.as_deref(), Some("BTCUSD still trade."));
        // Entries reopen Sunday 23:00 UTC: 1 day 11 hours after Saturday noon.
        assert_eq!(
            items[0].until_ms,
            Some((SATURDAY_NOON + 35 * 3_600) * 1_000)
        );
        let none = window_advisories(at(SATURDAY_NOON), None, &[]);
        assert_eq!(none[0].detail, None);
    }

    #[test]
    fn friday_cutoff_rollover_and_open_hours() {
        // Friday 2026-09-25 20:00 UTC: after the 19:00 cutoff.
        let friday = window_advisories(at(1_790_366_400), None, &[]);
        assert_eq!(friday[0].id, "entry_cutoff");
        // Wednesday 2026-09-23 21:00 UTC: inside the rollover pause, which
        // ends at 22:15.
        let wednesday_2100 = 1_790_197_200;
        let rollover = window_advisories(at(wednesday_2100), None, &[]);
        assert_eq!(rollover[0].id, "rollover_pause");
        assert_eq!(
            rollover[0].until_ms,
            Some((wednesday_2100 + 75 * 60) * 1_000)
        );
        // Wednesday noon: nothing to report.
        assert!(window_advisories(at(wednesday_2100 - 9 * 3_600), None, &[]).is_empty());
    }

    #[test]
    fn failure_reasons_read_plainly() {
        assert_eq!(
            plain_reason("calendar unavailable: calendar feed unavailable: error"),
            "The news calendar could not be read."
        );
        assert_eq!(
            plain_reason("judgement unavailable: jev transport failed"),
            "The market-reading service did not answer."
        );
        assert_eq!(
            plain_reason("model unavailable: model proposal failed"),
            "The AI model did not answer."
        );
        assert_eq!(plain_reason("something else"), "something else");
    }

    #[test]
    fn severities_order_critical_first() {
        let mut items = [
            advisory("a", Severity::Info, "a", None, None),
            advisory("b", Severity::Critical, "b", None, None),
            advisory("c", Severity::Warning, "c", None, None),
        ];
        items.sort_by_key(|item| item.severity);
        assert_eq!(
            items.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        let wire = serde_json::to_value(&items[0]).expect("json");
        assert_eq!(wire["severity"], "critical");
        assert!(wire["untilMs"].is_null());
    }

    fn config() -> crate::config::ServiceConfig {
        crate::config::ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            _ => Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config")
    }

    #[actix_web::test]
    async fn halts_missing_broker_and_skipped_rounds_are_reported_most_severe_first() {
        let policy = crate::risk::RiskPolicy::default()
            .apply_patch(&crate::risk::RiskPolicyPatch {
                kill_switch: Some(true),
                ..crate::risk::RiskPolicyPatch::default()
            })
            .expect("policy");
        let audit = crate::audit::AuditRuntime::new(std::sync::Arc::new(
            crate::audit::MemoryTrail::default(),
        ));
        let state = AppState::new(config(), None, None, crate::risk::RiskGate::new(policy))
            .with_audit(Some(audit.clone()))
            .with_fixed_now(Some(SystemTime::now()));
        state.set_trading_enabled(false);
        audit
            .try_record(crate::audit::AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "unavailable", "reason": "calendar unavailable: feed down"}),
            ))
            .await;

        let items = collect(&state).await;
        let ids: Vec<&str> = items.iter().map(|item| item.id).collect();
        assert!(
            ids.starts_with(&["kill_switch", "trading_disabled", "broker_missing"]),
            "{ids:?}"
        );
        let skipped = items
            .iter()
            .find(|item| item.id == "autopilot_skipped")
            .expect("skipped round");
        assert_eq!(
            skipped.detail.as_deref(),
            Some("The news calendar could not be read.")
        );
        assert!(
            items
                .windows(2)
                .all(|pair| pair[0].severity <= pair[1].severity),
            "most severe first"
        );

        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(web::Data::new(state))
                .service(advisories),
        )
        .await;
        let response = actix_web::test::call_service(
            &app,
            actix_web::test::TestRequest::get()
                .uri("/advisories")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = actix_web::test::read_body_json(response).await;
        assert_eq!(body["items"][0]["id"], "kill_switch");
        assert_eq!(body["items"][0]["severity"], "critical");
        assert!(body["generatedAtMs"].as_i64().is_some());
    }

    #[actix_web::test]
    async fn an_old_skipped_round_is_not_reported() {
        let audit = crate::audit::AuditRuntime::new(std::sync::Arc::new(
            crate::audit::MemoryTrail::default(),
        ));
        audit
            .try_record(crate::audit::AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "unavailable", "reason": "model unavailable"}),
            ))
            .await;
        let later = SystemTime::now() + Duration::from_secs(3_600);
        let state = AppState::new(
            config(),
            None,
            None,
            crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
        )
        .with_audit(Some(audit))
        .with_fixed_now(Some(later));
        assert!(skipped_round(&state, later).await.is_none());
    }

    #[test]
    fn breakers_say_what_it_takes_to_resume() {
        let now = SATURDAY_NOON;
        let items = breaker_items((20.0, 25.0), (0.0, 27.8), Some(49.92), now);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "breaker_peak");
        assert_eq!(
            items[0].title,
            "New trades paused: 27.8% below the account's peak"
        );
        assert_eq!(
            items[0].detail.as_deref(),
            Some(
                "Limit is 25%. Resumes when equity is back above 37.44, or when the peak is reset."
            )
        );
        let daily = breaker_items((20.0, 25.0), (21.0, 5.0), None, now);
        assert_eq!(daily[0].id, "breaker_daily");
        // Resumes at the next UTC midnight.
        assert_eq!(
            daily[0].until_ms,
            Some((SATURDAY_NOON + 12 * 3_600) * 1_000)
        );
        let unknown_peak = breaker_items((0.0, 25.0), (0.0, 30.0), None, now);
        assert_eq!(unknown_peak[0].detail.as_deref(), Some("Limit is 25%."));
        assert!(breaker_items((20.0, 25.0), (1.0, 2.0), Some(40.0), now).is_empty());
        assert!(
            breaker_items((0.0, 0.0), (99.0, 99.0), Some(40.0), now).is_empty(),
            "a zero limit is off"
        );
    }

    #[test]
    fn the_broker_link_notice() {
        assert_eq!(link_item(false, true).expect("stale").id, "broker_stale");
        assert_eq!(
            link_item(false, false).expect("stale first").id,
            "broker_stale"
        );
        assert_eq!(link_item(true, false).expect("disarmed").id, "ea_disarmed");
        assert!(link_item(true, true).is_none());
    }

    #[test]
    fn closed_instruments_are_named_with_the_earliest_reopening() {
        assert!(closed_instruments_item(&[]).is_none());
        let one = closed_instruments_item(&[("SP500m".to_owned(), Some(100))]).expect("one");
        assert_eq!(one.title, "SP500m is closed");
        assert_eq!(one.until_ms, Some(100_000));
        let two = closed_instruments_item(&[
            ("SP500m".to_owned(), Some(200)),
            ("Nd100m".to_owned(), Some(100)),
        ])
        .expect("two");
        assert_eq!(two.title, "SP500m and Nd100m are closed");
        assert_eq!(two.until_ms, Some(100_000));
        let unknown = closed_instruments_item(&[("X".to_owned(), None)]).expect("unknown");
        assert_eq!(unknown.until_ms, None);
    }

    #[test]
    fn session_hours_and_cutoff_details() {
        let crypto = vec![
            Symbol::parse("BTCUSD").expect("symbol"),
            Symbol::parse("ETHUSD").expect("symbol"),
        ];
        // Friday 20:00 UTC with weekend symbols: the cutoff names them.
        let friday = window_advisories(at(1_790_366_400), None, &crypto);
        assert_eq!(
            friday[0].detail.as_deref(),
            Some("Friday cutoff. Open trades are still managed. BTCUSD and ETHUSD still trade.")
        );
        // Wednesday 03:00 UTC outside an 08-17 session window.
        let session = crate::risk::SessionWindow::parse("8-17").expect("window");
        let early = window_advisories(at(1_790_197_200 - 18 * 3_600), Some(session), &[]);
        assert_eq!(early[0].id, "session_closed");
        assert!(early[0].until_ms.is_some());
    }
}
